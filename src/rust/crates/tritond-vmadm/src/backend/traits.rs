// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The host abstraction the lifecycle orchestrators drive. `HostBackend`
//! (V-7) wires these to real `zonecfg`/`zoneadm`/`zfs`/`svccfg`; `FakeHost`
//! (tests) records the ordered calls and simulates state. Keeping the
//! external effects behind a trait makes the create/rollback flow fully
//! unit-testable without a live CN, and is the seam for a future
//! alternate backend.

use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::Result;

/// How to address a zone on the `zonecfg`/`zoneadm` command line. For
/// vmadm-managed zones the zonename equals the uuid, but node uses `-z`
/// for some operations and `-u` for others; preserving that lets the
/// differential harness match argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZoneRef {
    Zonename(String),
    Uuid(Uuid),
}

/// Arguments to `zoneadm ... install`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallArgs {
    pub zonename: String,
    /// `-q <GiB>`.
    pub quota_gib: u64,
    /// `-t <image_uuid>` (OS zoneroot clone); `None` for bhyve.
    pub image_uuid: Option<Uuid>,
    /// `-x nodataset`.
    pub no_dataset: bool,
}

/// The outcome of waiting for the in-zone provisioning marker to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionOutcome {
    Success,
    Failure(String),
    Timeout,
}

/// A zone's runtime facts from `zoneadm list -p`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneRuntime {
    pub zoneid: Option<i64>,
    pub state: String,
    pub brand: String,
    pub zonepath: String,
    /// Zone debug id (`zoneadm list -p` field 8), `None` when not running.
    pub zonedid: Option<i64>,
}

/// The per-zone running facts node reads from the `zones:::zone_misc` kstat.
/// Only available for a `running` zone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ZoneKstat {
    /// `init_pid` (node `pid`).
    pub init_pid: Option<i64>,
    /// `init_restarts`.
    pub init_restarts: Option<u64>,
    /// `boot_time` as a unix timestamp in seconds (node `boot_timestamp`).
    pub boot_time: Option<i64>,
}

/// Host-wide facts node pulls from `sysinfo` for every VM object.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostFacts {
    /// sysinfo `UUID` (node `server_uuid`).
    pub server_uuid: Option<Uuid>,
    /// sysinfo `Live Image` (node `platform_buildstamp`).
    pub platform_buildstamp: Option<String>,
}

/// One zoneroot snapshot from `zfs list -t snapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// Full snapshot name (`<dataset>@<snapname>`).
    pub name: String,
    /// `used` in bytes.
    pub used: u64,
    /// `creation` as a unix timestamp in seconds.
    pub creation: i64,
}

impl ZoneRuntime {
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.state == "running"
    }
}

#[async_trait]
pub trait ZoneHost: Send + Sync {
    /// Whether a zone with this uuid is already configured (`zoneadm
    /// list`). Create's uniqueness guard.
    async fn zone_configured(&self, uuid: Uuid) -> Result<bool>;

    /// Pipe a `zonecfg` command stream to `zonecfg -z|-u <zone>`.
    async fn zonecfg(&self, zone: ZoneRef, doc: &str) -> Result<()>;

    /// `zoneadm -z <zonename> install [-q <gib>] [-t <image>] [-x nodataset]`.
    async fn zoneadm_install(&self, args: &InstallArgs) -> Result<()>;

    /// `zoneadm -u <uuid> boot`.
    async fn zoneadm_boot(&self, uuid: Uuid) -> Result<()>;

    /// `zfs create [-o k=v ...] <dataset>` (e.g. the delegated dataset).
    async fn zfs_create(&self, dataset: &str, props: &[(String, String)]) -> Result<()>;

    // ---- bhyve disk + quota primitives ----

    /// Whether a snapshot exists (`zfs get -Ho value name <snapshot>`).
    async fn zfs_snapshot_exists(&self, snapshot: &str) -> Result<bool>;

    /// Whether a dataset (filesystem/volume) exists
    /// (`zfs get -Ho value name <dataset>`, exit 0 == present). The
    /// reprovision data-preservation dance uses this to decide whether the
    /// zone has a delegated `/data` dataset to rename out of the way.
    async fn zfs_dataset_exists(&self, dataset: &str) -> Result<bool>;

    /// `zfs snapshot <snapshot>` (the `@final`-missing fallback).
    async fn zfs_snapshot(&self, snapshot: &str) -> Result<()>;

    /// `zfs clone -F [-o k=v ...] <snapshot> <dataset>` (boot/image zvol).
    async fn zfs_clone(
        &self,
        snapshot: &str,
        dataset: &str,
        props: &[(String, String)],
    ) -> Result<()>;

    /// `zfs create [-o k=v ...] -V <size_mib>M <dataset>` (blank data zvol).
    async fn zfs_create_volume(
        &self,
        dataset: &str,
        size_mib: u64,
        props: &[(String, String)],
    ) -> Result<()>;

    /// `zfs set k=v ... <dataset>` (the quota applier).
    async fn zfs_set(&self, dataset: &str, props: &[(String, String)]) -> Result<()>;

    /// `zfs list -Hrpt volume -o name,volsize <dataset>` -> (name, volsize).
    async fn zfs_list_volumes(&self, dataset: &str) -> Result<Vec<(String, u64)>>;

    /// `zfs list -Hpr -t volume -o refreservation <dataset>`, summed.
    async fn zfs_sum_volume_refreservation(&self, dataset: &str) -> Result<u64>;

    /// Build the NoCloud cidata FAT16 image on the (already-created) seed
    /// zvol at `raw_zvol_path` (`/dev/zvol/rdsk/...`) using the native
    /// `lofiadm`/`mkfs -F pcfs`/`mount` toolchain.
    async fn build_nocloud_seed(
        &self,
        raw_zvol_path: &str,
        files: &crate::seed::SeedFiles,
    ) -> Result<()>;

    /// `svccfg -s smartdc/mdata:execute setprop start/timeout_seconds`
    /// against the zone's offline repository under `zonepath`.
    async fn svccfg_mdata_exec_timeout(&self, zonepath: &str, secs: u64) -> Result<()>;

    /// Wait for `<zonepath>/root/var/svc/provisioning` to be replaced by
    /// `provision_success` / `provision_failure`, up to `timeout`.
    async fn wait_provisioning(
        &self,
        zonepath: &str,
        timeout: Duration,
    ) -> Result<ProvisionOutcome>;

    // ---- rollback / teardown primitives (tolerant of already-gone) ----

    /// `zfs destroy -rF <dataset>`; treats "does not exist" as success.
    async fn zfs_destroy_recursive(&self, dataset: &str) -> Result<()>;

    /// Every volume/snapshot dataset whose name is the flat-sibling disk
    /// `<prefix>` itself or begins with `<prefix>` (i.e. `<prefix>0`,
    /// `<prefix>1`, `<prefix>0@snap`). kvm makes its disks flat siblings of
    /// the zoneroot (`zones/<uuid>-disk0`, VM.js:3174), so they are NOT
    /// under the zoneroot subtree and survive the zoneroot recursive
    /// destroy. delete enumerates them with this and destroys each
    /// (node `deleteZone` -> `deleteVolume` per `vmobj.disks`, VM.js:13039).
    /// Backed by `zfs list -H -o name -t volume,snapshot` filtered to the
    /// prefix. An empty pool (no matches) yields an empty list, not an error.
    async fn zfs_list_sibling_disks(&self, disk_prefix: &str) -> Result<Vec<String>>;

    /// `zoneadm -u <uuid> uninstall -F`; tolerant of already-uninstalled.
    async fn zoneadm_uninstall_force(&self, uuid: Uuid) -> Result<()>;

    /// `zonecfg -u <uuid> delete -F`; tolerant of already-gone.
    async fn zonecfg_delete_force(&self, uuid: Uuid) -> Result<()>;

    // ---- lifecycle (start/stop/reboot/delete) + get/list primitives ----

    /// `zoneadm -z <uuid> list -p` parsed; `None` when not configured.
    async fn zone_info(&self, uuid: Uuid) -> Result<Option<ZoneRuntime>>;

    /// Every configured vmadm-managed zone uuid (`zoneadm list -cp`).
    async fn list_zone_uuids(&self) -> Result<Vec<Uuid>>;

    /// `zoneadm -u <uuid> halt -X` (forced power-off, bypassing brand
    /// statechange hooks). Tolerant of an already-halted zone.
    async fn zoneadm_halt(&self, uuid: Uuid) -> Result<()>;

    /// `zoneadm -u <uuid> reboot`.
    async fn zoneadm_reboot(&self, uuid: Uuid) -> Result<()>;

    /// Graceful in-guest shutdown of an OS zone
    /// (`zlogin <uuid> shutdown -y -i 5 -g 0`), bounded by `timeout`.
    async fn zlogin_shutdown(&self, uuid: Uuid, timeout: Duration) -> Result<()>;

    /// `zonecfg -u <uuid> export` (the zone config, for `get`/`list`).
    async fn zonecfg_export(&self, uuid: Uuid) -> Result<String>;

    /// `zfs get -Hp -o property,value <props> <dataset>` -> property map.
    async fn zfs_get_props(
        &self,
        dataset: &str,
        props: &[&str],
    ) -> Result<std::collections::BTreeMap<String, String>>;

    /// The `zones:::zone_misc` kstat facts for a running zone (`pid`,
    /// `init_restarts`, `boot_time`). An empty [`ZoneKstat`] for a zone with
    /// no kstat (not running) is fine; node simply omits those fields then.
    async fn zone_kstat(&self, zoneid: i64, zonename: &str) -> Result<ZoneKstat>;

    /// Host-wide `sysinfo` facts (`server_uuid`, `platform_buildstamp`).
    async fn host_facts(&self) -> Result<HostFacts>;

    /// `zfs list -Hpt snapshot -o name,used,creation -r <dataset>` — the
    /// zoneroot's snapshots (node filters to the `vmsnap-` prefix).
    async fn zfs_list_snapshots(&self, dataset: &str) -> Result<Vec<SnapshotInfo>>;

    // ---- holds / reprovision data-preservation / image-uuid edit ----

    /// `zfs holds -H <snapshot>` parsed into `(snapshot, tag)` pairs.
    /// Mirrors node `VM.js` `_listIndestructibleHolds` (lines 8093-8135):
    /// a missing snapshot ("does not exist" / "could not find") yields an
    /// empty list, not an error, so a destructibility check on a never-held
    /// dataset is a no-op.
    async fn zfs_holds(&self, snapshot: &str) -> Result<Vec<(String, String)>>;

    /// `zfs rename -f <from> <to>`. The reprovision flow renames the
    /// delegated dataset and zoneroot out of the way and back (node `VM.js`
    /// lines 11761/11799/12119).
    async fn zfs_rename(&self, from: &str, to: &str) -> Result<()>;

    /// `zfs set <prop>=<value> <dataset>` (single property). The reprovision
    /// flow toggles `zoned=off`/`zoned=on` on the delegated dataset (node
    /// `VM.js` lines 11713/12142).
    async fn zfs_set_prop(&self, dataset: &str, prop: &str, value: &str) -> Result<()>;

    /// `zonecfg -u <uuid> -f -` with `select attr name=<name>; set
    /// value="<value>"; end`. Updates the zone's `dataset-uuid` (node calls
    /// the attr `dataset-uuid` though the property is `image_uuid`, see
    /// `VM.js` lines 9710-9714, 12159-12160).
    async fn zonecfg_set_attr(&self, uuid: Uuid, name: &str, value: &str) -> Result<()>;

    /// `zonecfg -u <uuid> -f -` with `remove attr name=<name>`. The teardown
    /// counterpart to [`ZoneHost::zonecfg_set_attr`].
    async fn zonecfg_remove_attr(&self, uuid: Uuid, name: &str) -> Result<()>;

    /// `zonecfg -u <uuid> -f -` with `remove dataset name=<dataset>`.
    /// The reprovision JM path removes the delegated `dataset` resource
    /// before `zoneadm install`, because the install verify fails when the
    /// `dataset` named in the config has been parked out of the way (node
    /// `VM.js` does the equivalent rename dance around the re-install).
    /// Tolerant of an already-absent resource.
    async fn zonecfg_remove_dataset(&self, uuid: Uuid, dataset: &str) -> Result<()>;

    /// `zonecfg -u <uuid> -f -` with `add dataset; set name=<dataset>; end`.
    /// Re-adds the delegated `dataset` resource after the re-install and the
    /// `/data` restore (the counterpart to
    /// [`ZoneHost::zonecfg_remove_dataset`]). Idempotent: re-adding an
    /// already-present dataset resource is tolerated.
    async fn zonecfg_add_dataset(&self, uuid: Uuid, dataset: &str) -> Result<()>;
}

/// Registers a NIC's host link as a SmartOS nic_tag so the zone brand's
/// boot hook resolves the zonecfg `global-nic`. For a Proteus overlay
/// port this is mandatory before a joyent-minimal zone boots (else the
/// `jcommon/statechange` hook bails "undefined VNIC").
#[async_trait]
pub trait NicTagRegistrar: Send + Sync {
    /// Ensure `link` is a registered nic_tag (idempotent).
    async fn ensure(&self, link: &str) -> Result<()>;
}
