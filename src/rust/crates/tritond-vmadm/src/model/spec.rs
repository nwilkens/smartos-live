// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The normalized, brand-agnostic create model. Both engine fronts (the
//! drop-in binary's stdin JSON and the agent's in-process JSON) produce a
//! [`VmCreateSpec`] via [`crate::payload::normalize`]; the create
//! algorithm consumes only this.

use chrono::{DateTime, Utc};
use serde_json::Map;
use uuid::Uuid;

use crate::brand::Brand;
use crate::model::disk::DiskSpec;
use crate::model::nic::NicSpec;

pub type JsonMap = Map<String, serde_json::Value>;

#[derive(Debug, Clone)]
pub struct VmCreateSpec {
    pub uuid: Uuid,
    /// SmartOS zonename; equals `uuid` for vmadm-managed zones.
    pub zonename: String,
    pub brand: Brand,
    pub zpool: String,
    /// `{zpool}/{uuid}`.
    pub zfs_filesystem: String,
    /// `/{zfs_filesystem}`.
    pub zonepath: String,
    /// OS zones clone this image as the zoneroot. bhyve usually has no
    /// zoneroot image (the boot disk carries the image).
    pub image_uuid: Option<Uuid>,
    /// lx `kernel-version` attr (the Linux ABI version the brand emulates).
    /// `Some` only for `Brand::Lx`; the lx brand reads this attr at install
    /// time (proptable.js:1058, VM.js:6954).
    pub kernel_version: Option<String>,
    pub create_timestamp: DateTime<Utc>,
    pub vm_version: u32,
    pub autoboot: bool,
    pub create_only: bool,
    pub nowait: bool,

    // sizing
    pub quota_gib: u64,
    pub ram_mib: u64,
    pub max_physical_memory_mib: u64,
    pub max_locked_memory_mib: u64,
    pub max_swap_mib: u64,
    pub tmpfs_mib: Option<u64>,
    pub vcpus: u32,
    pub cpu_cap: Option<u32>,
    pub cpu_shares: u32,
    pub zfs_io_priority: u32,
    pub max_lwps: u32,
    pub max_msg_ids: u32,
    pub max_sem_ids: u32,
    pub max_shm_ids: u32,
    /// `max_shm_memory` in MiB (emitted to zonecfg as bytes).
    pub max_shm_memory_mib: u64,
    pub flexible_disk_size_mib: Option<u64>,
    pub bootrom: Option<String>,
    /// HVM `vnc-port` attr (0 = random, -1 = disabled). bhyve only.
    pub vnc_port: Option<i64>,
    /// HVM `boot` order attr. bhyve only.
    pub boot: Option<String>,

    // identity / misc
    pub limit_priv: Vec<String>,
    pub dns_domain: String,
    pub hostname: Option<String>,
    pub alias: Option<String>,
    pub owner_uuid: Uuid,
    pub billing_id: Uuid,
    pub resolvers: Vec<String>,
    pub routes: JsonMap,
    pub firewall_enabled: bool,
    pub maintain_resolvers: bool,
    pub do_not_inventory: bool,
    pub delegate_dataset: bool,
    pub indestructible_zoneroot: bool,
    pub indestructible_delegated: bool,
    pub mdata_exec_timeout_secs: u64,

    pub customer_metadata: JsonMap,
    pub internal_metadata: JsonMap,
    pub tags: JsonMap,

    /// The `docker` flag (proptable.js:798): a docker zone is an lx (or
    /// joyent-minimal) zone with this set true plus `docker:*`
    /// internal_metadata, run by the in-zone dockerinit. NOT a brand
    /// (src/dockerinit/README.md:13). When true the create path derives the
    /// docker-specific fields below (VM.js:10085-10148).
    pub docker: bool,
    /// `restart-init` zonecfg attr (VM.js:6949). docker forces this false so
    /// zoneadmd does not restart the container init (OS-3546, VM.js:10108).
    /// `None` means the attr is not emitted (node default restart_init=true).
    pub restart_init: Option<bool>,
    /// `init-name` zonecfg attr (VM.js:6950). docker sets it to the brand's
    /// dockerinit path (VM.js:10116-10122). `None` means the brand default.
    pub init_name: Option<String>,
    /// `internal-metadata-namespaces` zonecfg attr (VM.js:6962). docker adds
    /// `docker` so the `docker:*` keys are GZ-controlled and immutable from
    /// inside the zone (VM.js:10097-10103).
    pub internal_metadata_namespaces: Vec<String>,
    /// `zlog-mode` zonecfg attr for docker zones (buildDockerLogZonecfg,
    /// VM.js:6596). `Some` only when `docker` is set; computed from
    /// `docker:tty` + `docker:logdriver` (calculateZlogModeAndDriver,
    /// VM.js:19523). `None` for non-docker zones (no zlog-mode emitted),
    /// except bhyve which sets its own platform-log zlog-mode in build.rs.
    pub zlog_mode: Option<String>,
    /// `app-svc-dependent` zonecfg attr (VM.js:6635): emitted `true` when a
    /// docker zone uses a logger-backed log driver (not json-file/none), so
    /// init dies when the logger does. `false` otherwise.
    pub app_svc_dependent: bool,

    pub nics: Vec<NicSpec>,
    pub disks: Vec<DiskSpec>,
    pub bhyve_extra_opts: Option<String>,

    /// `Some` when the guest wants a NoCloud cloud-init seed disk (the
    /// `org.smartos:cloudinit_datasource == "nocloud"` /
    /// `internal_metadata.cloudinit_datasource == "nocloud"` opt-in).
    pub nocloud: bool,
}

impl VmCreateSpec {
    /// The host link names every NIC will land on, for the pre-boot
    /// nic_tag precondition check.
    #[must_use]
    pub fn nic_tags(&self) -> Vec<&str> {
        self.nics
            .iter()
            .filter_map(|n| n.nic_tag.as_deref())
            .collect()
    }
}
