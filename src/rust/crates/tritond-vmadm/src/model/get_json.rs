// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The `vmadm get` output model.
//!
//! This is the brand-agnostic VM JSON that node `vmadm get` (vmload)
//! assembles from `zoneadm`, `zonecfg export`, the per-zone config files,
//! and `zfs get`. We keep it a *superset* of the legacy shape on purpose:
//! real consumers (tritonagent's disk-resize `VmInfo`, the cn-platform
//! `vmadm::load`) deserialize a small slice of it with `#[serde(default)]`,
//! so emitting extra fields is always safe but a missing-or-mistyped field
//! they need is not.
//!
//! All the byte-vs-MiB-vs-GiB unit choices here match node-vmadm exactly,
//! because downstream code does arithmetic on them (e.g. tritonagent grows
//! `flexible_disk_size` in MiB and a disk `size` in MiB).

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Map;
use uuid::Uuid;

use crate::model::spec::JsonMap;

/// Serialize a `DateTime<Utc>` exactly as node's `Date.toISOString()` does:
/// millisecond precision and a literal `Z` (`2026-06-23T17:00:27.000Z`).
/// chrono's default RFC3339 serializer auto-scales the fractional digits, so
/// we format explicitly to stay byte-identical to node-vmadm.
mod node_iso8601 {
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde::Serializer;

    pub fn serialize<S: Serializer>(dt: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&dt.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    pub mod option {
        use super::*;

        pub fn serialize<S: Serializer>(
            dt: &Option<DateTime<Utc>>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match dt {
                Some(dt) => super::serialize(dt, s),
                None => s.serialize_none(),
            }
        }
    }
}

/// One disk in the `disks` array.
///
/// `path` / `boot` / `size` are the fields tritonagent's resize path reads;
/// the remainder round out parity with node so a generic `vmadm get`
/// consumer is not surprised by a missing key.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiskJson {
    /// Cooked block device path (`/dev/zvol/dsk/...`), the `match=` from the
    /// zonecfg `device` block.
    pub path: String,
    /// The zvol dataset name (`zones/<uuid>/disk0`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zfs_filesystem: Option<String>,
    pub boot: bool,
    /// Disk size in MiB. node stores `size` for blank disks and
    /// `image_size` for image-backed ones; we always surface a `size` so a
    /// consumer never has to special-case the image form.
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_uuid: Option<Uuid>,
    /// `refreservation` in bytes (from `zfs get`), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refreservation: Option<u64>,
    /// `volblocksize` in bytes, when set on the zonecfg device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_size: Option<u64>,
}

/// One NIC in the `nics` array. Reconstructed from the zonecfg `net`
/// resource; addresses are in the modern `ips`/`gateways` form.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct NicJson {
    pub interface: String,
    pub mac: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nic_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vlan_id: Option<u16>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ips: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gateways: Vec<String>,
    /// Singular ip/netmask/gateway node emits alongside ips/gateways.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub netmask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub primary: bool,
}

/// One snapshot in the `snapshots` array (node's `vmsnap-`-prefixed
/// zoneroot snapshots, in the friendly form vmadm exposes).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SnapshotJson {
    pub name: String,
    /// Snapshot `used` size in MiB.
    pub size: u64,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "node_iso8601::option::serialize"
    )]
    pub created_at: Option<DateTime<Utc>>,
}

/// The full `vmadm get` document.
///
/// Serializes with field names matching node-vmadm's keys. Optional fields
/// that node omits when absent are skipped so the JSON shape stays close to
/// the legacy output. node only includes `disks`/`do_not_inventory`/
/// `flexible_disk_size` for HVM (bhyve) zones, so those are `Option` and set
/// only for HVM to match node's brand-conditional shape.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VmGetJson {
    pub uuid: Uuid,
    pub zonename: String,
    pub brand: String,
    /// vmadm-normalized state (`running` / `stopped` / `provisioning` /
    /// ...), mapped from the raw zoneadm state.
    pub state: String,
    /// The raw `zoneadm` state, retained verbatim (node exposes both).
    pub zone_state: String,
    pub zonepath: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zoneid: Option<i64>,
    /// The zone debug id from `zoneadm list -p` field 8 (running zones).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zonedid: Option<i64>,
    pub autoboot: bool,
    /// `hvm` is `true` for bhyve/kvm brands, `false` for OS zones.
    pub hvm: bool,

    /// Zoneroot quota in GiB (node reports `quota` in GiB).
    pub quota: u64,
    /// MiB. node only emits this for HVM zones; `None` for an OS zone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flexible_disk_size: Option<u64>,
    /// `max_physical_memory` in MiB.
    pub max_physical_memory: u64,
    /// Guest RAM in MiB (bhyve only; `None` for an OS zone).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram: Option<u64>,
    /// vCPU count (bhyve only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u32>,

    /// node only emits `do_not_inventory` when it is `true` (the attr is
    /// set). `None` (and thus omitted) otherwise, for any brand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub do_not_inventory: Option<bool>,

    // ---- rctl-derived resource controls (zonecfg `add rctl`) ----
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_shares: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_cap: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zfs_io_priority: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_lwps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_msg_ids: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_sem_ids: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_shm_ids: Option<u64>,
    /// MiB (node `unmangleMem`s the bytes rctl limit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_shm_memory: Option<u64>,
    /// MiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_locked_memory: Option<u64>,
    /// MiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_swap: Option<u64>,

    // ---- zonecfg attrs ----
    /// `limitpriv` zonecfg scalar (node `limit_priv`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_priv: Option<String>,
    /// `vm-version` attr (node `v`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v: Option<u64>,
    /// `tmpfs` attr in MiB (OS zones).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmpfs: Option<u64>,
    /// `firewall-enabled` attr (node always emits `false` when unset).
    pub firewall_enabled: bool,
    /// `dataset-uuid` attr (node `image_uuid` for OS zones).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_uuid: Option<Uuid>,

    // ---- zfs-derived ----
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zfs_filesystem: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zpool: Option<String>,
    /// Zoneroot `recordsize` in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zfs_root_recsize: Option<u64>,
    pub snapshots: Vec<SnapshotJson>,

    // ---- sysinfo-derived ----
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_uuid: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_buildstamp: Option<String>,

    // ---- kstat-derived (running zones only) ----
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init_restarts: Option<u64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "node_iso8601::option::serialize"
    )]
    pub boot_timestamp: Option<DateTime<Utc>>,

    pub customer_metadata: JsonMap,
    pub internal_metadata: JsonMap,
    pub tags: JsonMap,
    pub routes: JsonMap,
    /// node always emits `resolvers` (an empty array when none are set), so
    /// it is unconditionally serialized.
    pub resolvers: Vec<String>,

    pub nics: Vec<NicJson>,
    /// node only emits `disks` for HVM zones; `None` for OS zones (whose
    /// storage is the zoneroot, not zvols).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disks: Option<Vec<DiskJson>>,

    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "node_iso8601::option::serialize"
    )]
    pub create_timestamp: Option<DateTime<Utc>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "node_iso8601::option::serialize"
    )]
    pub last_modified: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns_domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_uuid: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing_id: Option<Uuid>,
}

impl VmGetJson {
    /// A minimal document for `uuid`, with the maps initialized empty so a
    /// caller can populate fields incrementally during reconstruction.
    #[must_use]
    pub fn skeleton(uuid: Uuid) -> Self {
        Self {
            uuid,
            zonename: uuid.to_string(),
            brand: String::new(),
            state: String::new(),
            zone_state: String::new(),
            zonepath: String::new(),
            zoneid: None,
            zonedid: None,
            autoboot: false,
            hvm: false,
            quota: 0,
            flexible_disk_size: None,
            max_physical_memory: 0,
            ram: None,
            vcpus: None,
            do_not_inventory: None,
            cpu_shares: None,
            cpu_cap: None,
            zfs_io_priority: None,
            max_lwps: None,
            max_msg_ids: None,
            max_sem_ids: None,
            max_shm_ids: None,
            max_shm_memory: None,
            max_locked_memory: None,
            max_swap: None,
            limit_priv: None,
            v: None,
            tmpfs: None,
            firewall_enabled: false,
            image_uuid: None,
            zfs_filesystem: None,
            zpool: None,
            zfs_root_recsize: None,
            snapshots: Vec::new(),
            server_uuid: None,
            platform_buildstamp: None,
            pid: None,
            init_restarts: None,
            boot_timestamp: None,
            customer_metadata: Map::new(),
            internal_metadata: Map::new(),
            tags: Map::new(),
            routes: Map::new(),
            resolvers: Vec::new(),
            nics: Vec::new(),
            disks: None,
            create_timestamp: None,
            last_modified: None,
            alias: None,
            hostname: None,
            dns_domain: None,
            owner_uuid: None,
            billing_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local mirror of tritonagent's private `VmInfo`/`VmDisk` (services/
    /// tritonagent/src/vmadm.rs near line 508). If `VmGetJson` ever drops or
    /// retypes a field that path reads, this deserialize fails to compile or
    /// fails at runtime — pinning the cross-crate contract from this side.
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

    #[test]
    fn serializes_into_tritonagent_vm_info() {
        let mut vm = VmGetJson::skeleton(
            "11111111-1111-1111-1111-111111111111"
                .parse()
                .expect("uuid"),
        );
        vm.brand = "bhyve".to_string();
        vm.flexible_disk_size = Some(20480);
        vm.disks = Some(vec![DiskJson {
            path: "/dev/zvol/dsk/zones/11111111-1111-1111-1111-111111111111/disk0".to_string(),
            zfs_filesystem: Some("zones/11111111-1111-1111-1111-111111111111/disk0".to_string()),
            boot: true,
            size: 20480,
            model: Some("virtio".to_string()),
            media: Some("disk".to_string()),
            image_uuid: None,
            refreservation: Some(21_474_836_480),
            block_size: Some(8192),
        }]);

        let json = serde_json::to_string(&vm).expect("serialize");
        let parsed: AgentVmInfo = serde_json::from_str(&json).expect("agent parse");
        assert_eq!(parsed.flexible_disk_size, 20480);
        assert_eq!(parsed.disks.len(), 1);
        assert_eq!(
            parsed.disks[0].path,
            "/dev/zvol/dsk/zones/11111111-1111-1111-1111-111111111111/disk0"
        );
        assert!(parsed.disks[0].boot);
        assert_eq!(parsed.disks[0].size, 20480);
    }

    #[test]
    fn flexible_disk_size_and_dni_hvm_conditional() {
        // node omits flexible_disk_size/do_not_inventory/disks for OS zones
        // and includes them for HVM. A bare skeleton (OS) omits all three.
        let os = VmGetJson::skeleton(Uuid::nil());
        let v = serde_json::to_value(&os).expect("to_value");
        assert!(v.get("flexible_disk_size").is_none());
        assert!(v.get("do_not_inventory").is_none());
        assert!(v.get("disks").is_none());

        // An HVM zone surfaces all three (the form tritonagent reads).
        let mut hvm = VmGetJson::skeleton(Uuid::nil());
        hvm.flexible_disk_size = Some(0);
        hvm.do_not_inventory = Some(false);
        hvm.disks = Some(Vec::new());
        let v = serde_json::to_value(&hvm).expect("to_value");
        assert_eq!(v["flexible_disk_size"], serde_json::json!(0));
        assert_eq!(v["do_not_inventory"], serde_json::json!(false));
        assert_eq!(v["disks"], serde_json::json!([]));
    }
}
