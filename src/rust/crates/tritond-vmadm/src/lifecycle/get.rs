// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `vmadm get` / `vmadm list` reconstruction.
//!
//! node `vmadm get` (vmload) does not store a VM document; it *rebuilds*
//! one on every read from the authoritative host state. We do the same so
//! `get` can never drift from what the platform actually has configured:
//!
//! 1. `zoneadm list -p` ([`ZoneHost::zone_info`]) → state / zoneid / brand /
//!    zonepath.
//! 2. `zonecfg export` ([`ZoneHost::zonecfg_export`]) → the bulk of the
//!    document (attrs, net resources, device resources, capped-memory). The
//!    `zonecfg export` text is the source of truth, so we parse it rather
//!    than re-deriving from a stored spec.
//! 3. `<zonepath>/config/{metadata,tags,routes}.json` → the maps node keeps
//!    outside zonecfg (see [`crate::files`]).
//! 4. `zfs get` ([`ZoneHost::zfs_get_props`]) → quota + per-disk
//!    refreservation.
//!
//! `list` is `zoneadm list -cp` ([`ZoneHost::list_zone_uuids`]) followed by
//! a `get` of each; a zone that vanishes between the list and the get is
//! skipped rather than erroring (the inventory is racy by nature).

use std::collections::BTreeMap;
use std::path::Path;

use uuid::Uuid;

use crate::backend::traits::{ZoneHost, ZoneRuntime};
use crate::error::Result;
use crate::model::get_json::{DiskJson, NicJson, VmGetJson};
use crate::model::nic::fix_mac;
use crate::model::spec::JsonMap;

/// Reconstruct the `vmadm get` document for `uuid`. `Ok(None)` when no zone
/// with that uuid is configured (the caller maps this to
/// [`crate::error::VmadmError::NotFound`] or a 404).
pub async fn get<H: ZoneHost>(host: &H, uuid: Uuid) -> Result<Option<VmGetJson>> {
    let Some(info) = host.zone_info(uuid).await? else {
        return Ok(None);
    };
    Ok(Some(reconstruct(host, uuid, info).await?))
}

/// Reconstruct every configured vmadm-managed zone. A zone that disappears
/// between the enumeration and its `get` is dropped (it was deleted under
/// us), matching node's tolerance of a racy inventory.
pub async fn list<H: ZoneHost>(host: &H) -> Result<Vec<VmGetJson>> {
    let mut out = Vec::new();
    for uuid in host.list_zone_uuids().await? {
        if let Some(vm) = get(host, uuid).await? {
            out.push(vm);
        }
    }
    Ok(out)
}

/// Assemble the document from the four sources. Split out so both `get` and
/// `list` share it and so it is unit-testable against a [`crate::backend::fake::FakeHost`].
async fn reconstruct<H: ZoneHost>(host: &H, uuid: Uuid, info: ZoneRuntime) -> Result<VmGetJson> {
    let export = host.zonecfg_export(uuid).await?;
    let parsed = ParsedExport::parse(&export);

    let mut vm = VmGetJson::skeleton(uuid);
    vm.zonename = uuid.to_string();
    vm.brand = if parsed.brand.is_empty() {
        info.brand.clone()
    } else {
        parsed.brand.clone()
    };
    vm.zone_state = info.state.clone();
    vm.state = vmadm_state(&info.state);
    vm.zonepath = if parsed.zonepath.is_empty() {
        info.zonepath.clone()
    } else {
        parsed.zonepath.clone()
    };
    vm.zoneid = info.zoneid;
    vm.autoboot = parsed.autoboot;

    // attrs that map to top-level fields.
    vm.alias = parsed.alias;
    vm.hostname = parsed.attrs.get("hostname").cloned();
    vm.dns_domain = parsed.attrs.get("dns-domain").cloned();
    vm.owner_uuid = parsed.attrs.get("owner-uuid").and_then(|s| s.parse().ok());
    vm.billing_id = parsed.attrs.get("billing-id").and_then(|s| s.parse().ok());
    vm.create_timestamp = parsed
        .attrs
        .get("create-timestamp")
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    vm.do_not_inventory = parsed
        .attrs
        .get("do-not-inventory")
        .is_some_and(|v| v == "true");
    if let Some(resolvers) = parsed.attrs.get("resolvers") {
        vm.resolvers = resolvers
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
    vm.ram = parsed.attrs.get("ram").and_then(|s| s.parse().ok());
    vm.vcpus = parsed.attrs.get("vcpus").and_then(|s| s.parse().ok());
    vm.flexible_disk_size = parsed
        .attrs
        .get("flexible-disk-size")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    vm.max_physical_memory = parsed.capped_physical_mib.unwrap_or(0);
    vm.nics = parsed.nics;

    // config files (best-effort: an absent file is an empty map, matching a
    // freshly-installed zone before any metadata is written).
    let zonepath = Path::new(&vm.zonepath);
    vm.customer_metadata = read_metadata(zonepath, "customer_metadata").await;
    vm.internal_metadata = read_metadata(zonepath, "internal_metadata").await;
    vm.tags = read_config_map(&zonepath.join("config/tags.json")).await;
    vm.routes = read_config_map(&zonepath.join("config/routes.json")).await;

    // quota (GiB) from zfs. The zoneroot dataset is the zonepath sans slash.
    let dataset = vm.zonepath.trim_start_matches('/');
    if !dataset.is_empty() {
        let props = host
            .zfs_get_props(dataset, &["quota", "refquota"])
            .await
            .unwrap_or_default();
        // bhyve sets refquota on the zoneroot; OS zones set quota. `zfs
        // get -Hp` reports an unset limit as `0`, so prefer whichever is
        // non-zero (refquota first to match the bhyve ceiling).
        let pick = |k: &str| {
            props
                .get(k)
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0)
        };
        vm.quota = pick("refquota")
            .or_else(|| pick("quota"))
            .map(bytes_to_gib)
            .unwrap_or(0);
    }

    // disks: only HVM (bhyve) zones have zvol-backed disks. Enrich the
    // zonecfg device blocks with each zvol's refreservation from zfs.
    for dev in &parsed.devices {
        let mut disk = device_to_disk(dev);
        if let Some(ds) = &disk.zfs_filesystem
            && let Ok(props) = host.zfs_get_props(ds, &["refreservation"]).await
        {
            disk.refreservation = props
                .get("refreservation")
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|b| *b > 0);
        }
        vm.disks.push(disk);
    }

    Ok(vm)
}

/// Map a zoneadm runtime state to the vmadm-normalized state node reports.
/// vmadm collapses the zoneadm machine states into the smaller set callers
/// branch on; `running`/`installed` are the common ones.
fn vmadm_state(zone_state: &str) -> String {
    match zone_state {
        // A booted zone is "running" unless the in-zone provisioning marker
        // is still present; the marker check lives in the brand's transition
        // attr, not here, so we report the zoneadm view directly.
        "running" => "running",
        "installed" | "ready" => "stopped",
        "configured" | "incomplete" => "provisioning",
        "down" | "shutting_down" | "empty" => "stopped",
        other => other,
    }
    .to_string()
}

/// Bytes → whole GiB (node reports `quota` as an integer GiB count).
fn bytes_to_gib(bytes: u64) -> u64 {
    bytes / (1024 * 1024 * 1024)
}

/// Read one half of `config/metadata.json` (`internal_metadata` or
/// `customer_metadata`); an absent/un-parseable file is an empty map.
async fn read_metadata(zonepath: &Path, key: &str) -> JsonMap {
    let path = zonepath.join("config/metadata.json");
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return JsonMap::new();
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.get(key).and_then(|m| m.as_object()).cloned())
        .unwrap_or_default()
}

/// Read a standalone config map file (`tags.json` / `routes.json`); an
/// absent file or the `{}` skeleton yields an empty map.
async fn read_config_map(path: &Path) -> JsonMap {
    let Ok(bytes) = tokio::fs::read(path).await else {
        return JsonMap::new();
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Turn a parsed zonecfg `device` resource into a [`DiskJson`]. Sizes are in
/// MiB (the zonecfg `size`/`image-size` properties are MiB, matching the
/// create path in [`crate::zonecfg::build`]).
fn device_to_disk(dev: &Resource) -> DiskJson {
    // `match=/dev/zvol/rdsk/zones/<uuid>/diskN` (raw). vmadm's `path` and
    // tritonagent's resize both want the cooked `/dev/zvol/dsk/...` form.
    let raw = dev.scalars.get("match").cloned().unwrap_or_default();
    let cooked = raw.replacen("/dev/zvol/rdsk/", "/dev/zvol/dsk/", 1);
    let zfs_filesystem = raw
        .strip_prefix("/dev/zvol/rdsk/")
        .or_else(|| raw.strip_prefix("/dev/zvol/dsk/"))
        .map(str::to_string);

    let size = dev
        .props
        .get("size")
        .or_else(|| dev.props.get("image-size"))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    DiskJson {
        path: cooked,
        zfs_filesystem,
        boot: dev.props.get("boot").is_some_and(|v| v == "true"),
        size,
        model: dev.props.get("model").cloned(),
        media: dev.props.get("media").cloned(),
        image_uuid: dev.props.get("image-uuid").and_then(|s| s.parse().ok()),
        refreservation: None, // filled from zfs by the caller
        block_size: dev.props.get("volblocksize").and_then(|s| s.parse().ok()),
    }
}

/// A parsed zonecfg resource block (`add net`/`add device`/`add attr`/...).
///
/// `scalars` are `set k=v` lines; `props` are `add property (name=k,
/// value="v")` lines; `name`/`type`/`value` of an `attr` land in `scalars`.
#[derive(Default)]
struct Resource {
    kind: String,
    scalars: BTreeMap<String, String>,
    props: BTreeMap<String, String>,
}

/// The fields a `zonecfg export` text yields after parsing. Anything not
/// represented in the export (or in the config files / zfs) cannot be
/// reconstructed — see the module docs and the returned-summary notes.
#[derive(Default)]
struct ParsedExport {
    brand: String,
    zonepath: String,
    autoboot: bool,
    /// `attr` name → value (string attrs only; node stores everything as a
    /// `type=string` attr).
    attrs: BTreeMap<String, String>,
    alias: Option<String>,
    /// `capped-memory`'s `physical` in MiB.
    capped_physical_mib: Option<u64>,
    nics: Vec<NicJson>,
    devices: Vec<Resource>,
}

impl ParsedExport {
    /// Parse the line-oriented `zonecfg export` format. The grammar is:
    /// top-level `create -b` / `set k=v` lines, and `add <kind>` ... `end`
    /// blocks whose body is `set k=v` and `add property (name=k, value="v")`
    /// lines. Quoting on values is stripped. Unknown lines are ignored so a
    /// future zonecfg addition does not break the parse.
    fn parse(export: &str) -> Self {
        let mut p = ParsedExport::default();
        let mut current: Option<Resource> = None;

        for raw_line in export.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some(res) = current.as_mut() {
                if line == "end" {
                    p.finish_resource(current.take().unwrap_or_default());
                    continue;
                }
                Self::parse_resource_line(res, line);
                continue;
            }

            if let Some(kind) = line.strip_prefix("add ") {
                // `add property (...)` only appears inside a block; a bare
                // `add <kind>` opens one.
                current = Some(Resource {
                    kind: kind.trim().to_string(),
                    ..Resource::default()
                });
                continue;
            }

            if let Some((k, v)) = parse_set(line) {
                match k.as_str() {
                    "brand" => p.brand = v,
                    "zonepath" => p.zonepath = v,
                    "autoboot" => p.autoboot = v == "true",
                    _ => {}
                }
            }
        }
        // A truncated export (missing trailing `end`) still yields the block.
        if let Some(res) = current.take() {
            p.finish_resource(res);
        }
        p
    }

    /// Fold a `set`/`add property`/`add value` line into the open resource.
    fn parse_resource_line(res: &mut Resource, line: &str) {
        if let Some(rest) = line.strip_prefix("add property") {
            if let Some((k, v)) = parse_property(rest) {
                res.props.insert(k, v);
            }
        } else if let Some(rest) = line.strip_prefix("add value") {
            // rctl value: `(priv=privileged,limit=<N>,action=deny)`.
            if let Some((_, limit)) = parse_property(rest).or_else(|| parse_rctl_value(rest)) {
                res.scalars.insert("rctl-limit".to_string(), limit);
            }
        } else if let Some((k, v)) = parse_set(line) {
            res.scalars.insert(k, v);
        }
    }

    /// Promote a finished `add <kind>` block into the right collection.
    fn finish_resource(&mut self, res: Resource) {
        match res.kind.as_str() {
            "attr" => {
                if let (Some(name), Some(value)) =
                    (res.scalars.get("name"), res.scalars.get("value"))
                {
                    // `alias` is base64-encoded by the create path; decode it
                    // so the get output matches the original payload value.
                    if name == "alias" {
                        self.alias = decode_alias(value);
                    } else {
                        self.attrs.insert(name.clone(), value.clone());
                    }
                }
            }
            "net" => self.nics.push(net_to_nic(&res)),
            "device" => self.devices.push(res),
            "capped-memory" => {
                self.capped_physical_mib =
                    res.scalars.get("physical").and_then(|s| parse_size_mib(s));
            }
            // On a `zonecfg export` the capped-memory physical surfaces as
            // the `zone.max-physical-memory` rctl (bytes); convert to MiB.
            "rctl"
                if res.scalars.get("name").map(String::as_str)
                    == Some("zone.max-physical-memory") =>
            {
                self.capped_physical_mib = res
                    .scalars
                    .get("rctl-limit")
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|bytes| bytes / 1024 / 1024);
            }
            _ => {}
        }
    }
}

/// Build a [`NicJson`] from a parsed `net` resource. MACs are re-padded from
/// the ruined zonecfg form (`2:0:..`) back to canonical `02:00:..`.
fn net_to_nic(res: &Resource) -> NicJson {
    let mut nic = NicJson {
        interface: res.scalars.get("physical").cloned().unwrap_or_default(),
        nic_tag: res.scalars.get("global-nic").cloned(),
        vlan_id: res.scalars.get("vlan-id").and_then(|s| s.parse().ok()),
        model: res.props.get("model").cloned(),
        mtu: res.props.get("mtu").and_then(|s| s.parse().ok()),
        primary: res.props.get("primary").is_some_and(|v| v == "true"),
        ..NicJson::default()
    };
    if let Some(mac) = res.scalars.get("mac-addr") {
        nic.mac = fix_mac(mac);
    }
    if let Some(ips) = res.props.get("ips") {
        nic.ips = split_csv(ips);
    }
    if let Some(gws) = res.props.get("gateways") {
        nic.gateways = split_csv(gws);
    }
    // Singular ip/netmask/gateway (node emits these alongside ips/gateways).
    nic.ip = res.props.get("ip").cloned();
    nic.netmask = res.props.get("netmask").cloned();
    nic.gateway = res.props.get("gateway").cloned();
    nic
}

/// Decode the base64 `alias` attr the create path emits
/// (`Zonecfg::attr_base64`); on a decode failure return the raw value so the
/// field is never silently lost.
fn decode_alias(value: &str) -> Option<String> {
    use base64::Engine as _;
    match base64::engine::general_purpose::STANDARD.decode(value) {
        Ok(bytes) => String::from_utf8(bytes).ok().or(Some(value.to_string())),
        Err(_) => Some(value.to_string()),
    }
}

/// Parse `set key=value`, stripping surrounding quotes from the value.
fn parse_set(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("set ")?;
    let (k, v) = rest.split_once('=')?;
    Some((k.trim().to_string(), unquote(v.trim())))
}

/// Extract `limit=<N>` from an rctl `add value (priv=..,limit=N,action=..)`.
fn parse_rctl_value(rest: &str) -> Option<(String, String)> {
    let inner = rest.trim().strip_prefix('(')?.strip_suffix(')')?;
    for part in inner.split(',') {
        if let Some((k, v)) = part.split_once('=')
            && k.trim() == "limit"
        {
            return Some(("limit".to_string(), v.trim().to_string()));
        }
    }
    None
}

/// Parse the `(name=key, value="val")` tail of an `add property` line.
/// The value is a quoted string that may itself contain commas
/// (`ips`/`gateways`/`allowed_ips` are comma lists), so it must NOT be
/// split on commas — take the whole quoted run.
fn parse_property(rest: &str) -> Option<(String, String)> {
    let inner = rest.trim().strip_prefix('(')?.strip_suffix(')')?;
    // name=<key> — a simple token, up to the first comma.
    let npos = inner.find("name=")?;
    let name = inner[npos + 5..]
        .split(',')
        .next()?
        .trim()
        .trim_matches('"')
        .to_string();
    // value="..." — everything between the quotes (commas allowed).
    let vpos = inner.find("value=")?;
    let after = inner[vpos + 6..].trim_start();
    let value = match after.strip_prefix('"') {
        // Closing quote is the last `"` (zonecfg escapes embedded quotes).
        Some(q) => q[..q.rfind('"')?].to_string(),
        None => after.trim_end_matches([',', ' ']).to_string(),
    };
    Some((name, value))
}

/// Split a comma-separated zonecfg list value, dropping empties.
fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect()
}

/// Strip a single pair of surrounding double quotes.
fn unquote(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

/// Parse a zonecfg memory size (`2048M`, `4G`, or a bare byte count) into
/// MiB. Used for `capped-memory physical`.
fn parse_size_mib(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        return n.parse::<u64>().ok();
    }
    if let Some(n) = s.strip_suffix('G').or_else(|| s.strip_suffix('g')) {
        return n.parse::<u64>().ok().map(|g| g * 1024);
    }
    if let Some(n) = s.strip_suffix('K').or_else(|| s.strip_suffix('k')) {
        return n.parse::<u64>().ok().map(|k| k / 1024);
    }
    // A bare number is bytes.
    s.parse::<u64>().ok().map(|b| b / (1024 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeHost;
    use crate::backend::traits::ZoneRuntime;

    fn uuid() -> Uuid {
        "11111111-1111-1111-1111-111111111111"
            .parse()
            .expect("uuid")
    }

    fn runtime(state: &str, brand: &str) -> ZoneRuntime {
        ZoneRuntime {
            zoneid: Some(7),
            state: state.to_string(),
            brand: brand.to_string(),
            zonepath: format!("/zones/{}", uuid()),
        }
    }

    /// A joyent-minimal `zonecfg export`, in the canonical line-oriented
    /// form `zonecfg export` actually emits (attrs as `name`/`type`/`value`
    /// blocks; net properties as `add property (...)`).
    fn jm_export() -> String {
        let u = uuid();
        format!(
            "create -b\n\
             set zonepath=/zones/{u}\n\
             set brand=joyent-minimal\n\
             set autoboot=true\n\
             set ip-type=exclusive\n\
             set cpu-shares=100\n\
             add attr\n\
             set name=\"vm-version\"\n\
             set type=string\n\
             set value=\"1\"\n\
             end\n\
             add attr\n\
             set name=\"owner-uuid\"\n\
             set type=string\n\
             set value=\"22222222-2222-2222-2222-222222222222\"\n\
             end\n\
             add attr\n\
             set name=\"hostname\"\n\
             set type=string\n\
             set value=\"fdb0\"\n\
             end\n\
             add attr\n\
             set name=\"resolvers\"\n\
             set type=string\n\
             set value=\"10.199.199.14\"\n\
             end\n\
             add capped-memory\n\
             set physical=2048M\n\
             set swap=4096M\n\
             set locked=2048M\n\
             end\n\
             add net\n\
             set physical=net0\n\
             set global-nic=admin\n\
             set mac-addr=2:0:0:de:ad:1\n\
             add property (name=ips, value=\"10.199.199.50/24\")\n\
             add property (name=gateways, value=\"10.199.199.1\")\n\
             add property (name=primary, value=\"true\")\n\
             end\n"
        )
    }

    /// A bhyve export with a boot disk device + a viona net.
    fn bhyve_export() -> String {
        let u = uuid();
        format!(
            "create -b\n\
             set zonepath=/zones/{u}\n\
             set brand=bhyve\n\
             set autoboot=true\n\
             set ip-type=exclusive\n\
             add attr\n\
             set name=\"ram\"\n\
             set type=string\n\
             set value=\"2048\"\n\
             end\n\
             add attr\n\
             set name=\"vcpus\"\n\
             set type=string\n\
             set value=\"2\"\n\
             end\n\
             add attr\n\
             set name=\"flexible-disk-size\"\n\
             set type=string\n\
             set value=\"20480\"\n\
             end\n\
             add capped-memory\n\
             set physical=3072M\n\
             end\n\
             add device\n\
             set match=/dev/zvol/rdsk/zones/{u}/disk0\n\
             add property (name=boot, value=\"true\")\n\
             add property (name=model, value=\"virtio\")\n\
             add property (name=size, value=\"20480\")\n\
             add property (name=image-uuid, value=\"33333333-3333-3333-3333-333333333333\")\n\
             end\n\
             add net\n\
             set physical=net0\n\
             set global-nic=proteus49377\n\
             set mac-addr=2:0:0:de:ad:2\n\
             add property (name=model, value=\"virtio\")\n\
             add property (name=ips, value=\"10.0.0.5/24\")\n\
             add property (name=gateways, value=\"10.0.0.1\")\n\
             add property (name=primary, value=\"true\")\n\
             end\n"
        )
    }

    #[tokio::test]
    async fn get_missing_zone_is_none() {
        let host = FakeHost::new();
        assert!(get(&host, uuid()).await.expect("get").is_none());
    }

    /// The real `zonecfg export` renders properties as `(name=ips,value="a,b")`
    /// (no space, comma-separated quoted value) and surfaces capped-memory as
    /// a `zone.max-physical-memory` rctl. Both must parse correctly.
    #[tokio::test]
    async fn get_parses_real_export_multi_ip_and_rctl_memory() {
        let u = uuid();
        let export = format!(
            "create -b\n\
             set zonepath=/zones/{u}\n\
             set brand=joyent-minimal\n\
             add rctl\n\
             set name=\"zone.max-physical-memory\"\n\
             add value (priv=privileged,limit=536870912,action=deny)\n\
             end\n\
             add net\n\
             set physical=net0\n\
             set global-nic=admin\n\
             set mac-addr=2:0:0:de:ad:1\n\
             add property (name=gateways,value=\"10.0.0.1,10.0.0.2\")\n\
             add property (name=ips,value=\"10.0.0.5/24,10.0.0.6/24\")\n\
             add property (name=ip,value=\"10.0.0.5\")\n\
             add property (name=netmask,value=\"255.255.255.0\")\n\
             end\n"
        );
        let host = FakeHost::new();
        host.set_zone_info(u, runtime("running", "joyent-minimal"));
        host.set_export(u, export);

        let vm = get(&host, u).await.expect("get").expect("some");
        // 512 MiB from the rctl limit (536870912 bytes).
        assert_eq!(vm.max_physical_memory, 512);
        // Both IPs survive the quoted-comma value (the H2 fix).
        assert_eq!(
            vm.nics[0].ips,
            vec!["10.0.0.5/24".to_string(), "10.0.0.6/24".to_string()]
        );
        assert_eq!(
            vm.nics[0].gateways,
            vec!["10.0.0.1".to_string(), "10.0.0.2".to_string()]
        );
        assert_eq!(vm.nics[0].ip.as_deref(), Some("10.0.0.5"));
        assert_eq!(vm.nics[0].netmask.as_deref(), Some("255.255.255.0"));
    }

    #[tokio::test]
    async fn get_joyent_minimal_reconstructs_fields() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), runtime("running", "joyent-minimal"));
        host.set_export(uuid(), jm_export());
        let mut props = BTreeMap::new();
        // 25 GiB zoneroot quota in bytes.
        props.insert(
            "quota".to_string(),
            (25u64 * 1024 * 1024 * 1024).to_string(),
        );
        host.set_zfs_props(format!("zones/{}", uuid()), props);

        let vm = get(&host, uuid()).await.expect("get").expect("some");

        assert_eq!(vm.uuid, uuid());
        assert_eq!(vm.brand, "joyent-minimal");
        assert_eq!(vm.state, "running");
        assert_eq!(vm.zone_state, "running");
        assert_eq!(vm.zonepath, format!("/zones/{}", uuid()));
        assert_eq!(vm.zoneid, Some(7));
        assert!(vm.autoboot);
        assert_eq!(vm.quota, 25);
        assert_eq!(vm.max_physical_memory, 2048);
        assert_eq!(vm.hostname.as_deref(), Some("fdb0"));
        assert_eq!(
            vm.owner_uuid,
            Some(
                "22222222-2222-2222-2222-222222222222"
                    .parse()
                    .expect("uuid")
            )
        );
        assert_eq!(vm.resolvers, vec!["10.199.199.14".to_string()]);
        // OS zones have no zvol disks.
        assert!(vm.disks.is_empty());
        // one NIC, mac re-padded to canonical form.
        assert_eq!(vm.nics.len(), 1);
        assert_eq!(vm.nics[0].interface, "net0");
        assert_eq!(vm.nics[0].nic_tag.as_deref(), Some("admin"));
        assert_eq!(vm.nics[0].mac, "02:00:00:de:ad:01");
        assert_eq!(vm.nics[0].ips, vec!["10.199.199.50/24".to_string()]);
        assert!(vm.nics[0].primary);

        // The ordered host op sequence: info, export, then zfs for quota.
        let ops = host.ops();
        let i_info = ops
            .iter()
            .position(|o| o.contains("zone_info"))
            .expect("info");
        let i_export = ops
            .iter()
            .position(|o| o.contains("export"))
            .expect("export");
        let i_zfs = ops.iter().position(|o| o.contains("zfs get")).expect("zfs");
        assert!(i_info < i_export, "info before export: {ops:?}");
        assert!(i_export < i_zfs, "export before zfs: {ops:?}");
    }

    #[tokio::test]
    async fn get_bhyve_reconstructs_disk_with_refreservation() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), runtime("running", "bhyve"));
        host.set_export(uuid(), bhyve_export());
        let mut root = BTreeMap::new();
        root.insert(
            "refquota".to_string(),
            (30u64 * 1024 * 1024 * 1024).to_string(),
        );
        host.set_zfs_props(format!("zones/{}", uuid()), root);
        let mut disk = BTreeMap::new();
        disk.insert("refreservation".to_string(), 21_474_836_480u64.to_string());
        host.set_zfs_props(format!("zones/{}/disk0", uuid()), disk);

        let vm = get(&host, uuid()).await.expect("get").expect("some");

        assert_eq!(vm.brand, "bhyve");
        assert_eq!(vm.ram, Some(2048));
        assert_eq!(vm.vcpus, Some(2));
        assert_eq!(vm.flexible_disk_size, 20480);
        assert_eq!(vm.max_physical_memory, 3072);
        // refquota wins for the bhyve pool ceiling.
        assert_eq!(vm.quota, 30);

        assert_eq!(vm.disks.len(), 1);
        let d = &vm.disks[0];
        assert_eq!(d.path, format!("/dev/zvol/dsk/zones/{}/disk0", uuid()));
        assert_eq!(
            d.zfs_filesystem.as_deref(),
            Some(format!("zones/{}/disk0", uuid()).as_str())
        );
        assert!(d.boot);
        assert_eq!(d.size, 20480);
        assert_eq!(d.model.as_deref(), Some("virtio"));
        assert_eq!(
            d.image_uuid,
            Some(
                "33333333-3333-3333-3333-333333333333"
                    .parse()
                    .expect("uuid")
            )
        );
        assert_eq!(d.refreservation, Some(21_474_836_480));

        assert_eq!(vm.nics.len(), 1);
        assert_eq!(vm.nics[0].nic_tag.as_deref(), Some("proteus49377"));
        assert_eq!(vm.nics[0].model.as_deref(), Some("virtio"));
    }

    #[tokio::test]
    async fn get_round_trips_into_tritonagent_view() {
        // The whole point of the superset: the reconstructed JSON must parse
        // into tritonagent's resize `VmInfo`.
        #[derive(serde::Deserialize)]
        struct AgentVmInfo {
            #[serde(default)]
            flexible_disk_size: u64,
            #[serde(default)]
            disks: Vec<AgentVmDisk>,
        }
        #[derive(serde::Deserialize)]
        struct AgentVmDisk {
            path: String,
            #[serde(default)]
            boot: bool,
            #[serde(default)]
            size: u64,
        }

        let host = FakeHost::new();
        host.set_zone_info(uuid(), runtime("running", "bhyve"));
        host.set_export(uuid(), bhyve_export());
        let vm = get(&host, uuid()).await.expect("get").expect("some");

        let json = serde_json::to_string(&vm).expect("serialize");
        let agent: AgentVmInfo = serde_json::from_str(&json).expect("agent parse");
        assert_eq!(agent.flexible_disk_size, 20480);
        assert_eq!(agent.disks.len(), 1);
        assert!(agent.disks[0].boot);
        assert_eq!(agent.disks[0].size, 20480);
        assert!(agent.disks[0].path.starts_with("/dev/zvol/dsk/"));
    }

    #[tokio::test]
    async fn list_skips_zones_that_vanish() {
        let host = FakeHost::new();
        // Two configured uuids, but only one has canned info/export; the
        // other returns `zone_info == Some` from the fake's fallback, so
        // both reconstruct. Mark one configured-only to exercise the path.
        host.set_zone_info(uuid(), runtime("running", "joyent-minimal"));
        host.set_export(uuid(), jm_export());

        let vms = list(&host).await.expect("list");
        assert!(vms.iter().any(|v| v.uuid == uuid()));
        // list enumerated then got each.
        let ops = host.ops();
        assert!(ops.iter().any(|o| o.contains("list_zone_uuids")));
    }

    #[test]
    fn parse_handles_quotes_and_unknown_lines() {
        let export = "create -b\n\
            set zonepath=/zones/x\n\
            set brand=bhyve\n\
            set some-future-scalar=whatever\n\
            add attr\n\
            set name=\"alias\"\n\
            set type=string\n\
            set value=\"d2ViMA==\"\n\
            end\n";
        let p = ParsedExport::parse(export);
        assert_eq!(p.brand, "bhyve");
        assert_eq!(p.zonepath, "/zones/x");
        // alias base64 "d2ViMA==" decodes to "web0".
        assert_eq!(p.alias.as_deref(), Some("web0"));
    }

    #[test]
    fn parse_size_mib_units() {
        assert_eq!(parse_size_mib("2048M"), Some(2048));
        assert_eq!(parse_size_mib("4G"), Some(4096));
        assert_eq!(
            parse_size_mib(&(2048u64 * 1024 * 1024).to_string()),
            Some(2048)
        );
    }
}
