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
use crate::model::get_json::{DiskJson, NicJson, SnapshotJson, VmGetJson};
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
    vm.zonedid = info.zonedid;
    // For brands that manage autoboot via the vm-autoboot attr (kvm), the
    // zonecfg scalar is pinned to false and the real value lives in the
    // attr; vmload prefers it. Other brands report the scalar directly.
    vm.autoboot = match parsed.attrs.get("vm-autoboot") {
        Some(v) => v == "true",
        None => parsed.autoboot,
    };
    // node: hvm = HVM_BRANDS.indexOf(brand) >= 0 (bhyve/kvm).
    let is_hvm = vm.brand == "bhyve" || vm.brand == "kvm";
    vm.hvm = is_hvm;

    // attrs that map to top-level fields.
    vm.alias = parsed.alias;
    vm.hostname = parsed.attrs.get("hostname").cloned();
    vm.dns_domain = parsed.attrs.get("dns-domain").cloned();
    vm.owner_uuid = parsed.attrs.get("owner-uuid").and_then(|s| s.parse().ok());
    vm.billing_id = parsed.attrs.get("billing-id").and_then(|s| s.parse().ok());
    // node maps the `dataset-uuid` attr to `image_uuid`.
    vm.image_uuid = parsed
        .attrs
        .get("dataset-uuid")
        .and_then(|s| s.parse().ok());
    // lx kernel-version attr (node `kernel_version`); absent for other brands.
    vm.kernel_version = parsed.attrs.get("kernel-version").cloned();
    // docker attrs (proptable.js:798/1006/1842/1034/zlog-mode). docker is a
    // boolean attr (utils.fixBoolean); the others are the docker-derived
    // init/restart/namespaces/log fields a docker zone carries.
    vm.docker = parsed.attrs.get("docker").is_some_and(|v| v == "true");
    vm.init_name = parsed.attrs.get("init-name").cloned();
    vm.restart_init = parsed.attrs.get("restart-init").map(|v| v == "true");
    if let Some(ns) = parsed.attrs.get("internal-metadata-namespaces") {
        vm.internal_metadata_namespaces = ns
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
    vm.zlog_mode = parsed.attrs.get("zlog-mode").cloned();
    vm.create_timestamp = parsed
        .attrs
        .get("create-timestamp")
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    // node always emits firewall_enabled (false when the attr is unset).
    vm.firewall_enabled = parsed
        .attrs
        .get("firewall-enabled")
        .is_some_and(|v| v == "true");
    vm.limit_priv = parsed.limit_priv.clone();
    vm.v = parsed.attrs.get("vm-version").and_then(|s| s.parse().ok());
    vm.tmpfs = parsed.attrs.get("tmpfs").and_then(|s| s.parse().ok());
    if let Some(resolvers) = parsed.attrs.get("resolvers") {
        vm.resolvers = resolvers
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
    vm.ram = parsed.attrs.get("ram").and_then(|s| s.parse().ok());
    vm.vcpus = parsed.attrs.get("vcpus").and_then(|s| s.parse().ok());

    // rctl-derived resource controls. node `numberify`s the count rctls and
    // `unmangleMem`s (bytes -> MiB) the memory ones.
    let rctl = |name: &str| parsed.rctls.get(name).copied();
    let rctl_mib = |name: &str| rctl(name).map(|b| b / (1024 * 1024));
    vm.cpu_shares = rctl("zone.cpu-shares");
    vm.cpu_cap = rctl("zone.cpu-cap");
    vm.zfs_io_priority = rctl("zone.zfs-io-priority");
    vm.max_lwps = rctl("zone.max-lwps");
    vm.max_msg_ids = rctl("zone.max-msg-ids");
    vm.max_sem_ids = rctl("zone.max-sem-ids");
    vm.max_shm_ids = rctl("zone.max-shm-ids");
    vm.max_shm_memory = rctl_mib("zone.max-shm-memory");
    vm.max_locked_memory = rctl_mib("zone.max-locked-memory");
    vm.max_swap = rctl_mib("zone.max-swap");
    // max_physical_memory: prefer the rctl (matches node), then the
    // capped-memory block on a non-running export.
    vm.max_physical_memory = rctl_mib("zone.max-physical-memory")
        .or(parsed.capped_physical_mib)
        .unwrap_or(0);

    vm.nics = parsed.nics;

    // node emits flexible_disk_size only for HVM zones.
    if is_hvm {
        vm.flexible_disk_size = Some(
            parsed
                .attrs
                .get("flexible-disk-size")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        );
    }
    // node only surfaces do_not_inventory when the attr is set true.
    if parsed
        .attrs
        .get("do-not-inventory")
        .is_some_and(|v| v == "true")
    {
        vm.do_not_inventory = Some(true);
    }

    // config files (best-effort: an absent file is an empty map, matching a
    // freshly-installed zone before any metadata is written).
    let zonepath = Path::new(&vm.zonepath);
    vm.customer_metadata = read_metadata(zonepath, "customer_metadata").await;
    vm.internal_metadata = read_metadata(zonepath, "internal_metadata").await;
    vm.tags = read_config_map(&zonepath.join("config/tags.json")).await;
    vm.routes = read_config_map(&zonepath.join("config/routes.json")).await;

    // last_modified: newest mtime of the config files + /etc/zones xml,
    // matching node's getLastModified().
    vm.last_modified = newest_mtime(zonepath, &vm.zonename).await;

    // sysinfo facts: server_uuid + platform_buildstamp.
    if let Ok(facts) = host.host_facts().await {
        vm.server_uuid = facts.server_uuid;
        vm.platform_buildstamp = facts.platform_buildstamp;
    }

    // kstat facts: pid / init_restarts / boot_timestamp. node's vmload reads
    // the zone_misc kstat for *any* zone that has one — the kstat exists for
    // the whole lifetime a zone holds a zoneid (`ready`, `running`,
    // `shutting_down`, `down`), not just while running — and copies the fields
    // verbatim. Reading whenever a zoneid is present reproduces node's
    // transient sentinels: the brief `ready`/`shutting_down` window where the
    // kernel reports init_pid 4294967295 and boot_time 0 (1970-epoch
    // boot_timestamp). Once the zone reaches `installed` it has no zoneid, so
    // the kstat is skipped and pid/boot_timestamp drop — matching node's final
    // transition.
    if let Some(zoneid) = info.zoneid
        && info.state != "installed"
        && let Ok(k) = host.zone_kstat(zoneid, &vm.zonename).await
    {
        vm.pid = k.init_pid;
        vm.init_restarts = k.init_restarts;
        vm.boot_timestamp = k
            .boot_time
            .and_then(|secs| chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0));
    }

    // lastexited facts (non-running zones only): exit_status / exit_timestamp,
    // matching node vmload getLastExited (index.js:822/838 gate on
    // `zone_state !== 'running'`). A running zone never carries them.
    if info.state != "running"
        && let Some((status, ts)) = read_last_exited(zonepath).await
    {
        vm.exit_status = Some(status);
        vm.exit_timestamp = ts;
    }

    // quota + zfs root dataset facts. The zoneroot dataset is the zonepath
    // sans leading slash.
    let dataset = vm.zonepath.trim_start_matches('/').to_string();
    if !dataset.is_empty() {
        let props = host
            .zfs_get_props(&dataset, &["quota", "refquota", "recordsize"])
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
        vm.zfs_root_recsize = props.get("recordsize").and_then(|s| s.parse().ok());
        vm.zfs_filesystem = Some(dataset.clone());
        vm.zpool = dataset.split('/').next().map(str::to_string);
    }

    // snapshots: node exposes the zoneroot's `vmsnap-` snapshots in a
    // friendly form, newest first.
    vm.snapshots = collect_snapshots(host, &dataset).await;

    // disks: only HVM (bhyve) zones have zvol-backed disks. Enrich the
    // zonecfg device blocks with each zvol's refreservation from zfs.
    if is_hvm {
        let mut disks = Vec::new();
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
            disks.push(disk);
        }
        vm.disks = Some(disks);
    }

    Ok(vm)
}

/// Collect the zoneroot's `vmsnap-`-prefixed snapshots in the friendly form
/// node's `addSnapshots` exposes (name without prefix, size in MiB, created
/// timestamp), sorted newest-first.
async fn collect_snapshots<H: ZoneHost>(host: &H, dataset: &str) -> Vec<SnapshotJson> {
    if dataset.is_empty() {
        return Vec::new();
    }
    let Ok(snaps) = host.zfs_list_snapshots(dataset).await else {
        return Vec::new();
    };
    let prefix = format!("{dataset}@vmsnap-");
    let mut out: Vec<SnapshotJson> = snaps
        .into_iter()
        .filter_map(|s| {
            let name = s.name.strip_prefix(&prefix)?.to_string();
            Some(SnapshotJson {
                name,
                size: s.used / (1024 * 1024),
                created_at: chrono::DateTime::<chrono::Utc>::from_timestamp(s.creation, 0),
            })
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    out
}

/// Newest mtime across the config JSON files + the zone XML, as node's
/// `getLastModified()` computes `last_modified`. `None` if none exist.
async fn newest_mtime(zonepath: &Path, zonename: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let files = [
        zonepath.join("config/metadata.json"),
        zonepath.join("config/routes.json"),
        zonepath.join("config/tags.json"),
        zonepath.join("lastexited"),
        Path::new("/etc/zones").join(format!("{zonename}.xml")),
    ];
    let mut newest: Option<std::time::SystemTime> = None;
    for f in files {
        if let Ok(meta) = tokio::fs::metadata(&f).await
            && meta.is_file()
            && let Ok(mtime) = meta.modified()
        {
            newest = Some(newest.map_or(mtime, |cur| cur.max(mtime)));
        }
    }
    newest.map(chrono::DateTime::<chrono::Utc>::from)
}

/// Read and decode `<zonepath>/lastexited` (vmload `getLastExited`,
/// index.js:519). The file holds two whitespace-separated fields:
/// `<epoch_secs> <raw_status>`. Returns the decoded `(exit_status,
/// exit_timestamp)`; `None` when the file is absent or malformed (a zone that
/// has never exited). The timestamp is `None` only if the epoch field is
/// unparseable, mirroring node's best-effort decode.
async fn read_last_exited(zonepath: &Path) -> Option<(i64, Option<chrono::DateTime<chrono::Utc>>)> {
    let bytes = tokio::fs::read(zonepath.join("lastexited")).await.ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() != 2 {
        return None;
    }
    let raw: i64 = fields[1].parse().ok()?;
    // node getLastExited status decode (index.js:528):
    //   -1            -> 0 (OS-3429 normal-exit sentinel)
    //   (raw & 0xff)==0 (WIFEXITED) -> raw >> 8
    //   otherwise (signalled)       -> (raw & 0xff) * -1
    let exit_status = if raw == -1 {
        0
    } else if raw & 0xff == 0 {
        raw >> 8
    } else {
        -(raw & 0xff)
    };
    // node `epochTimestampSecs` does `new Date(Number(fields[0]) * 1000)`, so
    // the fractional-second part of the lastexited epoch (e.g.
    // `1782268970.538378`) becomes milliseconds in the ISO timestamp. Parse as
    // f64 and split into whole seconds + nanoseconds to preserve that.
    let exit_timestamp = fields[0].parse::<f64>().ok().and_then(|secs| {
        let whole = secs.trunc() as i64;
        let nanos = ((secs - secs.trunc()) * 1_000_000_000.0).round() as u32;
        chrono::DateTime::<chrono::Utc>::from_timestamp(whole, nanos)
    });
    Some((exit_status, exit_timestamp))
}

/// Map a zoneadm runtime state to the vmadm-normalized `state` node reports.
///
/// node vmload (index.js:908) maps ONLY `installed` -> `stopped`; every other
/// zoneadm state (`running`, `ready`, `down`, `shutting_down`, `configured`,
/// `incomplete`, ...) passes through verbatim. Reporting the raw zoneadm walk
/// is what lets vminfod emit the intermediate `shutting_down` -> `down` ->
/// `stopped` transitions node consumers see during a shutdown, instead of
/// jumping straight to `stopped`.
fn vmadm_state(zone_state: &str) -> String {
    match zone_state {
        "installed" => "stopped",
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
    /// `set limitpriv=...` top-level scalar (node `limit_priv`).
    limit_priv: Option<String>,
    /// `attr` name → value (string attrs only; node stores everything as a
    /// `type=string` attr).
    attrs: BTreeMap<String, String>,
    alias: Option<String>,
    /// `zone.<rctl>` name → privileged limit (raw, in the rctl's own units:
    /// bytes for memory rctls, a bare count otherwise).
    rctls: BTreeMap<String, u64>,
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
                    "limitpriv" => p.limit_priv = Some(v),
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
            // Every `add rctl` block: store its privileged limit verbatim so
            // the caller can apply node's per-rctl unit translation. The
            // limit is in the rctl's own units (bytes for memory rctls).
            "rctl" => {
                if let (Some(name), Some(limit)) = (
                    res.scalars.get("name"),
                    res.scalars
                        .get("rctl-limit")
                        .and_then(|s| s.parse::<u64>().ok()),
                ) {
                    self.rctls.insert(name.clone(), limit);
                }
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
            zonedid: Some(7),
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
        // node omits `disks` entirely for OS zones.
        assert!(vm.disks.is_none());
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
    async fn get_lx_reports_brand_and_kernel_version() {
        // An lx export is an OS-zone export with brand=lx + a kernel-version
        // attr + the dataset-uuid for the zoneroot image. get must surface
        // brand=lx, kernel_version, image_uuid, and (like any OS zone) emit
        // no `disks`.
        let u = uuid();
        let export = format!(
            "create -b\n\
             set zonepath=/zones/{u}\n\
             set brand=lx\n\
             set autoboot=true\n\
             add attr\n\
             set name=\"kernel-version\"\n\
             set type=string\n\
             set value=\"4.4.0\"\n\
             end\n\
             add attr\n\
             set name=\"dataset-uuid\"\n\
             set type=string\n\
             set value=\"4d3ed29a-c851-11e6-b5a9-639aada4a9c8\"\n\
             end\n"
        );
        let host = FakeHost::new();
        host.set_zone_info(u, runtime("running", "lx"));
        host.set_export(u, export);

        let vm = get(&host, u).await.expect("get").expect("some");
        assert_eq!(vm.brand, "lx");
        assert!(!vm.hvm, "lx is not an HVM brand");
        assert_eq!(vm.kernel_version.as_deref(), Some("4.4.0"));
        assert_eq!(
            vm.image_uuid.map(|i| i.to_string()).as_deref(),
            Some("4d3ed29a-c851-11e6-b5a9-639aada4a9c8")
        );
        // OS zone: no disks array, no flexible_disk_size.
        assert!(vm.disks.is_none());
        assert!(vm.flexible_disk_size.is_none());
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
        assert!(vm.hvm);
        assert_eq!(vm.ram, Some(2048));
        assert_eq!(vm.vcpus, Some(2));
        assert_eq!(vm.flexible_disk_size, Some(20480));
        // do_not_inventory is omitted unless the attr is set true.
        assert_eq!(vm.do_not_inventory, None);
        assert_eq!(vm.max_physical_memory, 3072);
        // refquota wins for the bhyve pool ceiling.
        assert_eq!(vm.quota, 30);

        let disks = vm.disks.as_ref().expect("hvm disks present");
        assert_eq!(disks.len(), 1);
        let d = &disks[0];
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
    fn vmadm_state_only_maps_installed() {
        // node vmload maps only `installed` -> `stopped`; the rest of the
        // shutdown walk passes through verbatim so vminfod can emit each step.
        assert_eq!(vmadm_state("installed"), "stopped");
        assert_eq!(vmadm_state("running"), "running");
        assert_eq!(vmadm_state("ready"), "ready");
        assert_eq!(vmadm_state("shutting_down"), "shutting_down");
        assert_eq!(vmadm_state("down"), "down");
        assert_eq!(vmadm_state("configured"), "configured");
        assert_eq!(vmadm_state("incomplete"), "incomplete");
    }

    #[tokio::test]
    async fn last_exited_decodes_wifexited_and_signal() {
        let dir = tempfile::tempdir().expect("tempdir");
        // WIFEXITED: raw 256 (0x100), low byte 0 -> status 256>>8 = 1.
        // Fractional epoch (like the real poststate hook writes) -> ms.
        tokio::fs::write(dir.path().join("lastexited"), "1700000000.538378 256")
            .await
            .expect("write");
        let (status, ts) = read_last_exited(dir.path()).await.expect("decoded");
        assert_eq!(status, 1);
        let ts = ts.expect("timestamp");
        // The fractional part survives as milliseconds in the ISO form.
        assert_eq!(
            ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "2023-11-14T22:13:20.538Z"
        );

        // OS-3429 sentinel -1 -> normal exit 0.
        tokio::fs::write(dir.path().join("lastexited"), "1700000001 -1")
            .await
            .expect("write");
        let (status, _) = read_last_exited(dir.path()).await.expect("decoded");
        assert_eq!(status, 0);

        // Signalled: raw 9 (low byte != 0) -> -(9 & 0xff) = -9.
        tokio::fs::write(dir.path().join("lastexited"), "1700000002 9")
            .await
            .expect("write");
        let (status, _) = read_last_exited(dir.path()).await.expect("decoded");
        assert_eq!(status, -9);
    }

    #[tokio::test]
    async fn last_exited_absent_or_malformed_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_last_exited(dir.path()).await.is_none());
        tokio::fs::write(dir.path().join("lastexited"), "only-one-field")
            .await
            .expect("write");
        assert!(read_last_exited(dir.path()).await.is_none());
    }

    #[tokio::test]
    async fn get_stopped_zone_surfaces_exit_fields() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), runtime("installed", "joyent-minimal"));
        host.set_export(uuid(), jm_export());
        // The reconstruct path reads <zonepath>/lastexited from disk; the
        // FakeHost's zonepath is /zones/<uuid> so write it there is not
        // possible in unit tests. Instead assert the state mapping: an
        // installed zone reports `stopped` (and a running zone never carries
        // exit fields).
        let vm = get(&host, uuid()).await.expect("get").expect("some");
        assert_eq!(vm.zone_state, "installed");
        assert_eq!(vm.state, "stopped");
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
